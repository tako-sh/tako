# Build the website and emit blog OG endpoint images in dist/
og:
    cd "{{ source_directory() }}/../website" && bun run build

# Import a local image as blog hero (from ~/Downloads, or specify filename)
img file="":
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{ source_directory() }}/.."
    if [ -z "{{ file }}" ]; then
        shopt -s nullglob
        downloads=("$HOME"/Downloads/*.png)
        if [ ${#downloads[@]} -eq 0 ]; then
            echo "No .png found in ~/Downloads" >&2
            exit 1
        fi
        src=$(ls -t "${downloads[@]}" | head -1)
    else
        src="$HOME/Downloads/{{ file }}"
    fi
    mkdir -p "$root/website/src/assets/blog"
    max_w=1400
    tmp_src=$(mktemp /tmp/blog-img-XXXXXX).png
    cp "$src" "$tmp_src"
    # Resize width to max if needed (preserves original aspect ratio)
    cur_w=$(sips -g pixelWidth "$tmp_src" | tail -1 | awk '{print $2}')
    if [ "$cur_w" -gt "$max_w" ]; then
        sips --resampleWidth "$max_w" "$tmp_src" --out "$tmp_src" >/dev/null 2>&1
    fi
    oxipng -o 4 --strip safe "$tmp_src"
    id=$(shasum -a 256 "$tmp_src" | cut -c1-12)
    out="$root/website/src/assets/blog/${id}.png"
    mv "$tmp_src" "$out"
    rm "$src"
    # Find the most recent blog post and patch it
    blog_dir="$root/website/src/content/blog"
    post=$(ls -t "$blog_dir"/*.md 2>/dev/null | head -1)
    if [ -n "$post" ]; then
        # Insert hash into image: frontmatter
        sed -i '' 's/^image:.*$/image: '"$id"'/' "$post"
        # Remove IMAGE PROMPT comment block (and the blank line that follows it)
        awk '
            /^<!-- IMAGE PROMPT/ { in_block = 1; next }
            in_block && /^-->$/ { in_block = 0; skip_blank = 1; next }
            in_block { next }
            skip_blank && /^$/ { skip_blank = 0; next }
            { skip_blank = 0; print }
        ' "$post" > "$post.tmp" && mv "$post.tmp" "$post"
        echo "$id → $(basename "$post")"
    else
        echo -n "$id" | pbcopy
        echo "$id (copied)"
    fi
