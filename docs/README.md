# Infigraph Documentation

This directory contains the source for the Infigraph documentation site, built with [Just the Docs](https://just-the-docs.github.io/) theme and deployed to GitHub Pages.

## Local Development

### Prerequisites

- Ruby 3.0+
- Bundler

### Setup

```bash
cd docs
bundle install
```

### Build & Serve

```bash
bundle exec jekyll serve
```

The site will be available at `http://localhost:4000/infigraph/`

### Build for Production

```bash
bundle exec jekyll build --strict_front_matter
```

Output goes to `docs/_site/`

## Structure

```
docs/
├── _config.yml              Jekyll configuration (Just the Docs theme)
├── index.md                 Landing page (nav_order: 1)
├── getting-started.md       Getting started (auto-generated, nav_order: 2)
├── architecture.md          Architecture (auto-generated, nav_order: 3)
├── contributing.md          Contributing (auto-generated, nav_order: 4)
├── Gemfile                  Ruby dependencies
└── README.md                This file
```

**Auto-generated files** (derived from root sources):
- `getting-started.md` — Extracted from README.md Quick Start section
- `architecture.md` — Auto-generated from ARCHITECTURE.md
- `contributing.md` — Auto-generated from CONTRIBUTING.md

These files are in `.gitignore` and regenerated on every deployment.

## Adding a New Page

1. Create a new `.md` file in the `docs/` directory
2. Add YAML frontmatter with nav_order:
   ```yaml
   ---
   layout: default
   title: Page Title
   nav_order: 5
   ---
   ```
3. Write content in Markdown
4. Links use Jekyll's `link` filter: `[text]({% link page.md %})`

## Deployment

Documentation is automatically deployed to GitHub Pages when changes are pushed to `main`:
- Triggers on: changes to `docs/**`, `ARCHITECTURE.md`, `CONTRIBUTING.md`, or `.github/workflows/pages.yml`
- Workflow: `.github/workflows/pages.yml`
- Site URL: https://intuit.github.io/infigraph/

## Theme Configuration

Uses [Just the Docs](https://just-the-docs.github.io/) — professional documentation theme with:
- Sidebar navigation
- Built-in search
- Dark/light mode toggle
- Mobile responsive design
- Syntax highlighting

To customize:
- Edit `_config.yml` for site metadata, color scheme, and navigation links
- Theme styling is built-in; no custom CSS needed

## Single Source of Truth Pattern

README.md is the source of truth. Jekyll pages derive from it:
- Root `README.md` → docs pages via auto-generation
- No duplication: pages are ephemeral, regenerated on every build
- Edit root sources; Jekyll updates automatically

## References

- [Just the Docs Documentation](https://just-the-docs.github.io/)
- [Jekyll Documentation](https://jekyllrb.com/docs/)
- [GitHub Pages with Jekyll](https://docs.github.com/en/pages/setting-up-a-github-pages-site-with-jekyll)
