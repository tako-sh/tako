set working-directory := ".."

# Typecheck and build website, then check for broken internal links
website:
    bun run --filter website check
    bun run --filter website build
    lychee \
        --offline \
        --root-dir website/dist \
        'website/dist/**/*.html'
