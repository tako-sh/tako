FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    openssh-client \
    curl \
    unzip \
    jq \
    ca-certificates \
    git \
  && rm -rf /var/lib/apt/lists/*

ENV BUN_INSTALL=/root/.bun
RUN curl -fsSL https://bun.sh/install | bash \
  && ln -s "${BUN_INSTALL}/bin/bun" /usr/local/bin/bun

# Node.js + pnpm (for node fixtures)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
  && apt-get install -y --no-install-recommends nodejs \
  && npm install -g pnpm \
  && rm -rf /var/lib/apt/lists/*

# Go (for go fixtures)
RUN ARCH="$(dpkg --print-architecture)" \
  && curl --fail --show-error --location --retry 5 --retry-all-errors --retry-delay 2 \
    --output /tmp/go.tar.gz \
    "https://dl.google.com/go/go1.22.10.linux-${ARCH}.tar.gz" \
  && tar -C /usr/local -xzf /tmp/go.tar.gz \
  && rm /tmp/go.tar.gz \
  && ln -s /usr/local/go/bin/go /usr/local/bin/go
ENV PATH="/usr/local/go/bin:$PATH"

WORKDIR /workspace

COPY e2e/docker/runner/run-e2e.sh /usr/local/bin/run-e2e.sh
RUN chmod +x /usr/local/bin/run-e2e.sh

ENTRYPOINT ["/usr/local/bin/run-e2e.sh"]
