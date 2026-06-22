#!/usr/bin/env python3
"""
output_token_multiplier 实测标定工具
====================================

背景
----
本仓库是 Kiro 反代，**上游不返回 output token 真值**（只有 contextUsageEvent =
输入侧 %、metering = credits）。因此 `src/token.rs::output_token_multiplier`
里那串 1.37~1.67 的倍率，是把「内嵌 DeepSeek-V3 BPE 对输出文本的本地切分数」
放大到「Claude 真实 output_tokens」的纯手调魔数——也就是 memory 里 `别臆测`
点名的部分。

方法（对齐 javirandor/anthropic-tokenizer 的思路：拿 Claude 真值做基准）
----------------------------------------------------------------------
javirandor 用 echo 流式法从官方 API 取真实 token 边界。这里用**更便宜、更准**的
等价手段——官方 `/v1/messages/count_tokens`：它按模型真实分词器返回 input_tokens，
免 token 费、能覆盖 4.7+ 更密分词。对每个代表性输出样本 S 与每个模型 M：

    claude_tokens(S, M) = count_tokens([{user: S}], M) - overhead(M)

其中 overhead(M) 是 per-message 结构开销（BOS + role 包裹），用一个极短基线样本
测一次再扣除。本地 DeepSeek 切分数 deepseek_tokens(S) 用**与 Rust 完全一致**的
内嵌 tokenizer.json（add_special_tokens=False）算出。

倍率
----
output_token_multiplier 最终是乘到「整段输出的 DeepSeek 计数」上的单一标量，
故最优值 = 代表性语料上 `Σ claude_true / Σ deepseek`（语料加权，即混合口径）。
脚本同时给出分类别 ratio 便于观察纯代码/纯散文的偏向。

用法
----
    export ANTHROPIC_API_KEY=sk-ant-...
    pip install anthropic tokenizers
    python3 tools/calibrate_output_multiplier.py
    # 自定义样本（每行 {"text": ..., "category": ...}）：
    python3 tools/calibrate_output_multiplier.py --samples my_outputs.jsonl
    # 只标定部分模型：
    python3 tools/calibrate_output_multiplier.py --models claude-opus-4.8,claude-sonnet-4.6

输出：每模型 per-category 与语料加权 ratio，以及可直接粘进 token.rs 的 match 臂。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEEPSEEK_TOKENIZER = REPO_ROOT / "deepseek_v3_tokenizer" / "tokenizer.json"

# 本仓库 map_model 归一化 id → 官方 count_tokens 可用的 model id。
# 归一化 id 见 src/anthropic/converter.rs::map_model；
# 官方 id 见 claude-api skill / shared/models.md。
# 源 anthropic：直连官方 /v1/messages/count_tokens（需 ANTHROPIC_API_KEY）。
NORMALIZED_TO_API = {
    "claude-opus-4.8": "claude-opus-4-8",
    "claude-opus-4.7": "claude-opus-4-7",
    "claude-opus-4.6": "claude-opus-4-6",
    "claude-opus-4.5": "claude-opus-4-5",
    "claude-sonnet-4.6": "claude-sonnet-4-6",
    "claude-sonnet-4.5": "claude-sonnet-4-5",
    "claude-haiku-4.5": "claude-haiku-4-5",
}

# 源 claudetokenizer：POST https://www.claudetokenizer.com/api（免 key，后端代理官方
# count_tokens，返回真实 input_tokens）。其下拉列表只到 opus-4-7，故 4.8 用 4-7 代理
# （同分词器），4.5 用 4-6 代理（同为旧分词器）。代理项在输出里标 (proxy)。
CLAUDETOKENIZER_ENDPOINT = "https://www.claudetokenizer.com/api"
NORMALIZED_TO_CTOK = {
    "claude-opus-4.8": ("claude-opus-4-7", True),  # 站点无 4-8；与 4-7 同分词器
    "claude-opus-4.7": ("claude-opus-4-7", False),
    "claude-opus-4.6": ("claude-opus-4-6", False),
    "claude-opus-4.5": ("claude-opus-4-6", True),  # 站点无 4-5；同为旧分词器家族
    "claude-sonnet-4.6": ("claude-sonnet-4-6", False),
    "claude-sonnet-4.5": ("claude-sonnet-4-5-20250929", False),
    "claude-haiku-4.5": ("claude-haiku-4-5-20251001", False),
}

# 极短基线：用来测 per-message 结构开销。tokens("x") 在任何 BPE 下都是 1，
# 故 overhead(M) = count_tokens([{user:"x"}], M) - 1。
BASELINE_TEXT = "x"


def load_deepseek_counter():
    """返回与 Rust `token::count_tokens` 等价的本地计数函数。"""
    try:
        from tokenizers import Tokenizer
    except ImportError:
        sys.exit("缺少 tokenizers，请 `pip install tokenizers`")
    if not DEEPSEEK_TOKENIZER.exists():
        sys.exit(f"找不到内嵌 tokenizer：{DEEPSEEK_TOKENIZER}")
    tok = Tokenizer.from_file(str(DEEPSEEK_TOKENIZER))

    def count(text: str) -> int:
        if not text:
            return 0
        # 对齐 Rust：add_special_tokens=False，只数内容 token。
        return len(tok.encode(text, add_special_tokens=False).ids)

    # 自检：与 token.rs 的定点测试一致，确保切分口径没漂。
    assert count("Hello, world!") == 4, "DeepSeek 切分与 token.rs 定点不符"
    assert count("abcdefgh") == 2
    return count


def default_samples() -> list[dict]:
    """代表性**输出**样本：贴近 assistant 实际产出（代码/散文/中文/JSON 工具入参/混合）。"""
    code_rust = '''\
/// 对原始 token 估算应用 output 校正系数，向上取整、保底 1。
pub(crate) fn calibrate_output_tokens(raw: u64, model: &str) -> i32 {
    ((raw as f64 * output_token_multiplier(model)).round() as i64).max(1) as i32
}

fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tool_definition_tokens(tool);
        }
    }
    for msg in &messages {
        total += count_message_content_tokens(&msg.content);
        total += TOKENS_PER_MESSAGE_OVERHEAD;
    }
    total.max(1)
}'''

    code_ts = '''\
export async function callRemoteCountTokens(
  apiUrl: string,
  config: CountTokensConfig,
  payload: CountTokensRequest,
): Promise<number> {
  const res = await fetch(apiUrl, {
    method: "POST",
    headers: { "content-type": "application/json", "x-api-key": config.apiKey },
    body: JSON.stringify(payload),
  });
  if (!res.ok) throw new Error(`count_tokens ${res.status}`);
  const data = (await res.json()) as { input_tokens: number };
  return data.input_tokens;
}'''

    prose_en = (
        "The proxy currently embeds DeepSeek V3's byte-pair tokenizer as a stand-in "
        "for Claude's real tokenizer, then scales the result by a hand-tuned constant. "
        "Because the upstream never reports output token usage, that constant is the "
        "only thing standing between our local estimate and the number the billing "
        "side actually charges for. Measuring it against ground truth, rather than "
        "guessing, is the whole point of this exercise."
    )

    prose_zh = (
        "本地估算口径目前内嵌的是 DeepSeek V3 的子词分词器，作为 Claude 真实分词器的"
        "近似，再乘上一个手工标定的系数。由于上游从不返回输出侧的 token 用量，这个系数"
        "就是本地估算与计费方实际计量之间唯一的桥梁。用真值去测量它、而不是凭感觉拍脑袋，"
        "正是这次标定要解决的问题。代码、散文、中英文混排各自的压缩比并不相同，所以要用"
        "贴近真实输出的混合语料来取折中。"
    )

    json_tool = json.dumps(
        {
            "name": "edit_file",
            "input": {
                "file_path": "/Users/dev/project/src/anthropic/stream.rs",
                "old_string": "self.output_tokens = count_tokens(&self.output_buf);",
                "new_string": "self.output_tokens = calibrate_output_tokens(\n    count_tokens(&self.output_buf),\n    &self.model,\n);",
                "replace_all": False,
            },
        },
        ensure_ascii=False,
        indent=2,
    )

    markdown_mixed = '''\
## 修复说明

把 `output_token_multiplier` 从手调魔数换成**实测值**：

1. 用 `count_tokens` 对代表性输出取 Claude 真实 token 数。
2. 与内嵌 DeepSeek 切分数求比值 `ratio = true / deepseek`。
3. 语料加权得到每模型的单一倍率。

```rust
Some("claude-opus-4.8") => 1.67, // ← 替换为实测
```

> 注意：纯代码会略偏高、纯散文略偏低，混合口径取折中。'''

    return [
        {"category": "code", "text": code_rust},
        {"category": "code", "text": code_ts},
        {"category": "prose_en", "text": prose_en},
        {"category": "prose_zh", "text": prose_zh},
        {"category": "json_tool", "text": json_tool},
        {"category": "markdown", "text": markdown_mixed},
    ]


def load_samples(path: str | None) -> list[dict]:
    if not path:
        return default_samples()
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            obj = json.loads(line)
            out.append({"category": obj.get("category", "custom"), "text": obj["text"]})
    if not out:
        sys.exit(f"{path} 里没有可用样本")
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--samples", help="JSONL 样本文件，每行 {\"text\":..., \"category\":...}")
    ap.add_argument(
        "--models",
        help="逗号分隔的归一化模型 id 子集，默认全部：" + ",".join(NORMALIZED_TO_API),
    )
    ap.add_argument("--sleep", type=float, default=0.2, help="每次 API 调用间隔秒数（防限流）")
    ap.add_argument(
        "--source",
        choices=["claudetokenizer", "anthropic"],
        default="claudetokenizer",
        help="真值源：claudetokenizer=免 key 代理（默认）；anthropic=官方 count_tokens（需 key）",
    )
    args = ap.parse_args()

    deepseek_count = load_deepseek_counter()
    samples = load_samples(args.samples)

    # 按源构建 claude_count(text, api_model) 与归一化 id→api_model 的映射。
    if args.source == "anthropic":
        api_key = os.environ.get("ANTHROPIC_API_KEY")
        if not api_key:
            sys.exit("--source anthropic 需先 export ANTHROPIC_API_KEY=sk-ant-...")
        try:
            import anthropic
        except ImportError:
            sys.exit("缺少 anthropic SDK，请 `pip install anthropic`")
        client = anthropic.Anthropic(api_key=api_key)
        model_table = {k: (v, False) for k, v in NORMALIZED_TO_API.items()}

        def claude_count(text: str, api_model: str) -> int:
            resp = client.messages.count_tokens(
                model=api_model,
                messages=[{"role": "user", "content": text}],
            )
            time.sleep(args.sleep)
            return resp.input_tokens
    else:
        import urllib.error
        import urllib.request

        model_table = NORMALIZED_TO_CTOK

        def claude_count(text: str, api_model: str) -> int:
            body = json.dumps({"text": text, "model": api_model}).encode()
            req = urllib.request.Request(
                CLAUDETOKENIZER_ENDPOINT,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=30) as resp:
                data = json.loads(resp.read())
            time.sleep(args.sleep)
            # 站点返回原始 input_tokens（含 per-message overhead，由调用方各自扣除）。
            return int(data["input_tokens"])

    if args.models:
        models = [m.strip() for m in args.models.split(",") if m.strip()]
        for m in models:
            if m not in model_table:
                sys.exit(f"未知模型 id：{m}（可选：{','.join(model_table)}）")
    else:
        models = list(model_table)

    # 预存每个样本的 DeepSeek 计数（与模型无关）。
    for s in samples:
        s["deepseek"] = deepseek_count(s["text"])

    suggestions: dict[str, float] = {}

    for norm in models:
        api_model, is_proxy = model_table[norm]
        proxy_tag = "  (proxy)" if is_proxy else ""
        print(f"\n===== {norm}  (源模型: {api_model}){proxy_tag} =====")
        try:
            overhead = claude_count(BASELINE_TEXT, api_model) - deepseek_count(BASELINE_TEXT)
        except Exception as e:  # noqa: BLE001
            print(f"  ！基线调用失败，跳过该模型：{e}")
            continue
        print(f"  per-message 结构开销 overhead ≈ {overhead} token（已从每个样本扣除）")

        cat_true: dict[str, int] = {}
        cat_ds: dict[str, int] = {}
        total_true = total_ds = 0

        print(f"  {'category':<10} {'deepseek':>9} {'claude':>8} {'ratio':>7}")
        for s in samples:
            try:
                raw = claude_count(s["text"], api_model)
            except Exception as e:  # noqa: BLE001
                print(f"  ！样本调用失败：{e}")
                continue
            true = max(raw - overhead, 1)
            ds = s["deepseek"]
            ratio = true / ds if ds else float("nan")
            cat_true[s["category"]] = cat_true.get(s["category"], 0) + true
            cat_ds[s["category"]] = cat_ds.get(s["category"], 0) + ds
            total_true += true
            total_ds += ds
            print(f"  {s['category']:<10} {ds:>9} {true:>8} {ratio:>7.3f}")

        if total_ds == 0:
            continue
        print("  " + "-" * 38)
        for cat in sorted(cat_ds):
            r = cat_true[cat] / cat_ds[cat]
            print(f"  {cat:<10} {cat_ds[cat]:>9} {cat_true[cat]:>8} {r:>7.3f}  (类别加权)")
        corpus = total_true / total_ds
        suggestions[norm] = corpus
        print(f"  {'语料加权':<10} {total_ds:>9} {total_true:>8} {corpus:>7.3f}  ← 建议倍率")

    if suggestions:
        print("\n\n===== 可粘进 src/token.rs::output_token_multiplier 的 match 臂 =====")
        for norm, val in suggestions.items():
            print(f'        Some("{norm}") => {val:.2f},')
        print("\n注：样本越贴近你的真实输出分布，标定越准。建议用 --samples 喂实际流量样本复测。")


if __name__ == "__main__":
    main()
