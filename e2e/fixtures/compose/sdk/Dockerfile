FROM docker.io/oven/bun:1-alpine
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY
WORKDIR /app
COPY package.json tako-sdk.tgz* ./
RUN bun install --production
COPY . .
USER bun
