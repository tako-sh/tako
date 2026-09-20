FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    openssh-client \
    curl \
    unzip \
    jq \
    ca-certificates \
    git \
    libvips42 \
  && rm -rf /var/lib/apt/lists/*

ENV BUN_INSTALL=/root/.bun
RUN curl -fsSL https://bun.sh/install | bash \
  && ln -s "${BUN_INSTALL}/bin/bun" /usr/local/bin/bun

# Node.js + pnpm (for node fixtures)
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
  && apt-get install -y --no-install-recommends nodejs \
  && npm install -g pnpm \
  && rm -rf /var/lib/apt/lists/*

# Go (for go fixtures)
RUN ARCH="$(dpkg --print-architecture)" \
  && curl --fail --show-error --location --retry 5 --retry-all-errors --retry-delay 2 \
    --output /tmp/go.tar.gz \
    "https://dl.google.com/go/go1.26.5.linux-${ARCH}.tar.gz" \
  && tar -C /usr/local -xzf /tmp/go.tar.gz \
  && rm /tmp/go.tar.gz \
  && ln -s /usr/local/go/bin/go /usr/local/bin/go
ENV PATH="/usr/local/go/bin:$PATH"

WORKDIR /workspace

COPY e2e/docker/runner/run-e2e.sh /usr/local/bin/run-e2e.sh
RUN chmod +x /usr/local/bin/run-e2e.sh

ENTRYPOINT ["/usr/local/bin/run-e2e.sh"]
