# syntax=docker/dockerfile:1.28@sha256:bb22d9815c728170f72750f4e5b0d672e06176142e1d602c7e66c050100b7e5b

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8

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
