set working-directory := ".."

# Typecheck and build website, then check for broken internal links
website:
    bun run --filter website check
    bun run --filter website build
    lychee \
        --root-dir website/dist \
        --exclude 'pkg\.go\.dev' \
        --exclude 'npmjs\.com/package/tako\.sh' \
        --exclude 'x\.com/dhh' \
        --exclude 'tako\.sh/blog' \
        --exclude 'tako\.sh/docs' \
        'website/dist/**/*.html'
