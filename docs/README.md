# Infigraph Documentation

This directory contains the source for the Infigraph documentation site, built with Jekyll and deployed to GitHub Pages.

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
├── _config.yml           Jekyll configuration
├── assets/
│   └── css/style.scss    Custom stylesheet
├── index.md              Landing page
├── getting-started.md    Getting started guide
├── architecture.md       Architecture & design document
├── contributing.md       Contributing guidelines
└── Gemfile              Ruby dependencies
```

## Adding a New Page

1. Create a new `.md` file in the `docs/` directory
2. Add YAML frontmatter:
   ```yaml
   ---
   layout: default
   title: Page Title
   ---
   ```
3. Write content in Markdown
4. Link from other pages using `[text](/infigraph/page-slug)`

## Deployment

Documentation is automatically deployed to GitHub Pages when changes are pushed to `main` (triggered by updates to `docs/**` files).

Site URL: https://intuit.github.io/infigraph/

## Theme

Uses [jekyll-theme-minimal](https://github.com/pages-themes/minimal) with custom styling in `assets/css/style.scss`.

To modify the theme:
- Edit `_config.yml` for site metadata and navigation
- Edit `assets/css/style.scss` for styling
- Modify layouts in `_includes/` or `_layouts/` (copied from theme)

## References

- [Jekyll Documentation](https://jekyllrb.com/docs/)
- [GitHub Pages with Jekyll](https://docs.github.com/en/pages/setting-up-a-github-pages-site-with-jekyll)
- [Minimal Theme](https://pages-themes.github.io/minimal/)
