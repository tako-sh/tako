# syntax=docker/dockerfile:1.27@sha256:bde3983e9c939224420ddaf6b784cc30e09b035a4dea01f581230c50809f372e

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

# Lightweight SSH-accessible debug server image for installer/deploy testing.
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    bash \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Keep server dependency installation aligned with the production installer.
COPY scripts/install-tako-server.sh /tmp/install-tako-server.sh
RUN chmod +x /tmp/install-tako-server.sh && \
    printf '#!/bin/sh\nexit 0\n' > /tmp/tako-server && \
    chmod +x /tmp/tako-server && \
    TAKO_RESTART_SERVICE=0 TAKO_SERVER_URL="file:///tmp/tako-server" sh /tmp/install-tako-server.sh && \
    rm -f /tmp/install-tako-server.sh /tmp/tako-server

RUN ssh-keygen -A && \
    mkdir -p /var/run/sshd /run/sshd /root/.ssh && \
    chmod 700 /root/.ssh && \
    sed -i 's/^#\?PermitRootLogin .*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config && \
    sed -i 's/^#\?PubkeyAuthentication .*/PubkeyAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/^#\?PasswordAuthentication .*/PasswordAuthentication no/' /etc/ssh/sshd_config && \
    sed -i 's|^#\?AuthorizedKeysFile .*|AuthorizedKeysFile .ssh/authorized_keys|' /etc/ssh/sshd_config

COPY docker/install-authorized-key.sh /usr/local/bin/install-authorized-key
RUN chmod +x /usr/local/bin/install-authorized-key

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
