import tailwindcssAnimate from 'tailwindcss-animate'

/** @type {import('tailwindcss').Config} */
export default {
  darkMode: 'class',
  content: [
    './index.html',
    './src/**/*.{js,ts,jsx,tsx}',
  ],
  theme: {
    extend: {
      fontFamily: {
        sans: ['"Fira Sans"', 'system-ui', 'sans-serif'],
        mono: ['"Fira Code"', 'ui-monospace', 'SFMono-Regular', 'Menlo', 'monospace'],
      },
      fontSize: {
        '2xs': ['0.6875rem', { lineHeight: '1rem' }],
      },
      colors: {
        border: 'hsl(var(--border))',
        input: 'hsl(var(--input))',
        ring: 'hsl(var(--ring))',
        background: 'hsl(var(--background))',
        foreground: 'hsl(var(--foreground))',
        surface: 'hsl(var(--surface))',
        'surface-2': 'hsl(var(--surface-2))',
        primary: {
          DEFAULT: 'hsl(var(--primary))',
          foreground: 'hsl(var(--primary-foreground))',
          soft: 'hsl(var(--primary-soft))',
        },
        secondary: {
          DEFAULT: 'hsl(var(--secondary))',
          foreground: 'hsl(var(--secondary-foreground))',
        },
        cta: {
          DEFAULT: 'hsl(var(--cta))',
          foreground: 'hsl(var(--cta-foreground))',
        },
        destructive: {
          DEFAULT: 'hsl(var(--destructive))',
          foreground: 'hsl(var(--destructive-foreground))',
        },
        muted: {
          DEFAULT: 'hsl(var(--muted))',
          foreground: 'hsl(var(--muted-foreground))',
        },
        accent: {
          DEFAULT: 'hsl(var(--accent))',
          foreground: 'hsl(var(--accent-foreground))',
        },
        popover: {
          DEFAULT: 'hsl(var(--popover))',
          foreground: 'hsl(var(--popover-foreground))',
        },
        card: {
          DEFAULT: 'hsl(var(--card))',
          foreground: 'hsl(var(--card-foreground))',
        },
        ok: {
          DEFAULT: 'hsl(var(--ok))',
          foreground: 'hsl(var(--ok-foreground))',
          soft: 'hsl(var(--ok-soft))',
        },
        warn: {
          DEFAULT: 'hsl(var(--warn))',
          foreground: 'hsl(var(--warn-foreground))',
          soft: 'hsl(var(--warn-soft))',
        },
        bad: {
          DEFAULT: 'hsl(var(--bad))',
          foreground: 'hsl(var(--bad-foreground))',
          soft: 'hsl(var(--bad-soft))',
        },
      },
      borderRadius: {
        lg: 'var(--radius)',
        md: 'calc(var(--radius) - 2px)',
        sm: 'calc(var(--radius) - 4px)',
      },
      boxShadow: {
        card: '0 1px 2px 0 rgba(0, 0, 0, 0.04)',
        elev: '0 1px 3px 0 rgba(0, 0, 0, 0.08), 0 1px 2px -1px rgba(0, 0, 0, 0.06)',
        pop: '0 8px 24px -8px rgba(0, 0, 0, 0.18), 0 2px 6px -2px rgba(0, 0, 0, 0.08)',
        // Stripe 官方卡片阴影：第一层 1px 发丝边（即"边框"）+ navy 色调柔光
        stripe:
          '0 0 0 1px rgba(50, 50, 93, 0.1), 0 2px 5px 0 rgba(50, 50, 93, 0.08), 0 1px 1.5px 0 rgba(0, 0, 0, 0.07)',
        'stripe-lg':
          '0 0 0 1px rgba(50, 50, 93, 0.1), 0 7px 14px 0 rgba(50, 50, 93, 0.1), 0 3px 6px 0 rgba(0, 0, 0, 0.08)',
      },
    },
  },
  plugins: [tailwindcssAnimate],
}
