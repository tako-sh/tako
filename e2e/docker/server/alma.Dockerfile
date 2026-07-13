FROM almalinux:10@sha256:cc24bc5b6ac7e284f2f62a07bdaa1b15d3319fdcf46413c6b8fe9fa245068ddd

RUN dnf install -y \
      git \
      nmap-ncat \
      npm \
      which \
      openssh-server \
      openssh-clients \
      sudo \
      xz \
      zstd \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# Pre-install tako-server (with dummy binary, installer creates user/service)
COPY scripts/install-tako-server.sh /tmp/install-tako-server.sh
RUN chmod +x /tmp/install-tako-server.sh \
    && printf '#!/bin/sh\nexit 0\n' > /tmp/tako-server \
    && chmod +x /tmp/tako-server \
    && tar -cf - -C /tmp tako-server | zstd -o /tmp/tako-server.tar.zst \
    && sha256sum /tmp/tako-server.tar.zst | awk '{print $1}' > /tmp/tako-server.tar.zst.sha256 \
    && TAKO_SERVER_URL="file:///tmp/tako-server.tar.zst" TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /tmp/install-tako-server.sh \
    && rm -f /tmp/install-tako-server.sh /tmp/tako-server /tmp/tako-server.tar.zst /tmp/tako-server.tar.zst.sha256

# Generate SSH host keys
RUN ssh-keygen -A

COPY e2e/docker/server/entrypoint.sh /usr/local/bin/tako-e2e-entrypoint.sh
RUN chmod +x /usr/local/bin/tako-e2e-entrypoint.sh

EXPOSE 22

CMD ["/usr/local/bin/tako-e2e-entrypoint.sh"]
