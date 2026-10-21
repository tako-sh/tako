set working-directory := ".."

# Typecheck and build website, then check local links. Docs7 serves /docs separately.
website:
    bun run --filter website check
    bun run --filter website build
    lychee \
        --offline \
        --root-dir website/dist \
        --exclude 'file://.*/website/dist/docs(/|$)' \
        'website/dist/**/*.html'
