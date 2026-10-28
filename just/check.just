set working-directory := ".."

# Typecheck and build the complete website, then check all local links.
website:
    bun run --filter website check
    bun run --filter website build
    lychee \
        --offline \
        --root-dir website/dist \
        'website/dist/**/*.html'
