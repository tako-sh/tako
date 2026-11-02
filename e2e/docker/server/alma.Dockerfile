FROM docker:28.4.0-dind@sha256:2ceb471176ad51e37145d43ce7cbf0fa5d644a2b185bd537f0ef695fb3a37497 AS docker

FROM almalinux:10@sha256:5a4b41a427c9717be8f4ac2a32aa22efbae5f9124fbcb79a6765394f312acfe8

RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
    trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  fi; \
  sed -i -e 's/^mirrorlist=/#mirrorlist=/' -e 's/^# *baseurl=/baseurl=/' /etc/yum.repos.d/almalinux*.repo \
    && dnf install -y \
      git \
      iptables \
      nmap-ncat \
      npm \
      which \
      openssh-server \
      openssh-clients \
      procps-ng \
      sudo \
      xz \
      zstd \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# The dummy server below has no dynamic dependencies. Install the real server's
# image runtime here so every fixture does not need to bootstrap Remi again.
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  set -eu; \
  cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
  trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  if [ -s /run/secrets/proxy_ca ]; then \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
  fi; \
  printf '\nmax_parallel_downloads=1\n' >> /etc/dnf/dnf.conf; \
  dnf install -y epel-release dnf-plugins-core; \
  dnf config-manager --set-enabled crb; \
  dnf install -y --disablerepo='epel*' "https://rpms.remirepo.net/enterprise/remi-release-$(rpm -E %rhel).rpm"; \
  dnf config-manager --set-enabled remi; \
  dnf install -y --disablerepo='remi-modular*' --setopt=install_weak_deps=False vips; \
  dnf clean all; \
  rm -rf /var/cache/dnf


# The disposable host runs its own engine; never mount the executor's socket.
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker-proxy /usr/local/bin/docker-init /usr/local/bin/dockerd /usr/local/bin/containerd /usr/local/bin/containerd-shim-runc-v2 /usr/local/bin/runc /usr/local/bin/
COPY --from=docker /usr/local/libexec/docker/cli-plugins/ /usr/local/libexec/docker/cli-plugins/

# Pre-install tako-server (with dummy binary, installer creates user/service)
COPY scripts/install-tako-server.sh /tmp/install-tako-server.sh
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
    trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  fi; \
  chmod +x /tmp/install-tako-server.sh \
    && printf '#!/bin/sh\nexit 0\n' > /tmp/tako-server \
    && chmod +x /tmp/tako-server \
    && tar -cf - -C /tmp tako-server | zstd -o /tmp/tako-server.tar.zst \
    && sha256sum /tmp/tako-server.tar.zst | awk '{print $1}' > /tmp/tako-server.tar.zst.sha256 \
    && TAKO_SERVER_URL="file:///tmp/tako-server.tar.zst" TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /tmp/install-tako-server.sh \
    && rm -f /tmp/install-tako-server.sh /tmp/tako-server /tmp/tako-server.tar.zst /tmp/tako-server.tar.zst.sha256

# Generate SSH host keys
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
    trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  fi; \
  ssh-keygen -A

COPY e2e/docker/server/entrypoint.sh /usr/local/bin/tako-e2e-entrypoint.sh
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
    trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  fi; \
  chmod +x /usr/local/bin/tako-e2e-entrypoint.sh

EXPOSE 22

# The real installer checks already-installed system packages on every boot.
# Freeze metadata for this disposable image so those checks are deterministic;
# application dependencies and runtime downloads still use the normal network.
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  set -eu; \
  cp /etc/dnf/dnf.conf /tmp/e2e-dnf.conf; \
  trap 'cp /tmp/e2e-dnf.conf /etc/dnf/dnf.conf; printf "\ncacheonly=True\n" >> /etc/dnf/dnf.conf; rm -f /tmp/e2e-dnf.conf' EXIT; \
  if [ -s /run/secrets/proxy_ca ]; then \
    printf '\nsslcacert=/run/secrets/proxy_ca\n' >> /etc/dnf/dnf.conf; \
  fi; \
  dnf config-manager --set-disabled 'remi-modular*'; \
  dnf makecache; \
  dnf clean packages

CMD ["/usr/local/bin/tako-e2e-entrypoint.sh"]
