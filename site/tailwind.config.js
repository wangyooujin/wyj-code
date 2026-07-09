/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ['./index.html', './script.js'],
  theme: {
    extend: {
      colors: {
        ink: {
          950: '#08090a',
          900: '#0b0d0e',
          850: '#101315',
          800: '#14171a',
          700: '#1b1f23',
          600: '#262b30',
          500: '#3a4148',
        },
        paper: '#f3efe4',
        amber: {
          400: '#ffc571',
          500: '#ffb454',
          600: '#f2973a',
          700: '#c9701f',
        },
        phosphor: '#7ee8b8',
      },
      fontFamily: {
        display: ['"JetBrains Mono"', 'ui-monospace', 'monospace'],
        mono: ['"IBM Plex Mono"', 'ui-monospace', 'monospace'],
      },
      backgroundImage: {
        grain: "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='120' height='120'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='2' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)' opacity='0.4'/%3E%3C/svg%3E\")",
        grid: "linear-gradient(to right, rgba(255,255,255,0.035) 1px, transparent 1px), linear-gradient(to bottom, rgba(255,255,255,0.035) 1px, transparent 1px)",
      },
      backgroundSize: {
        gridsize: '40px 40px',
      },
      keyframes: {
        blink: { '0%,49%': { opacity: 1 }, '50%,100%': { opacity: 0 } },
        rise: { '0%': { opacity: 0, transform: 'translateY(14px)' }, '100%': { opacity: 1, transform: 'translateY(0)' } },
        scan: { '0%': { backgroundPosition: '0 0' }, '100%': { backgroundPosition: '0 40px' } },
      },
      animation: {
        blink: 'blink 1s step-end infinite',
        rise: 'rise 0.7s cubic-bezier(0.16,1,0.3,1) both',
        scan: 'scan 3s linear infinite',
      },
    },
  },
  plugins: [],
};
