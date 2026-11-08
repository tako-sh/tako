FROM oven/bun:1-alpine@sha256:4cf82f5686b196504fe119db3fcd9185b191f82ece82c40680e597228ec6070a
WORKDIR /app
COPY --chown=bun:bun server.ts ./server.ts
USER bun
EXPOSE 3000
CMD ["bun", "server.ts"]
