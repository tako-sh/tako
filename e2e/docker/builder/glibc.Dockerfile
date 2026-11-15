FROM rust:1.98-bullseye@sha256:4730e387a220a08a365c77da3096544dde214f9d796c16284d4be45438cad4a9

# Bullseye keeps the deployment binary's glibc floor low. Its security
# packages moved to the archive after LTS; retain signed archive metadata.
# Its CMake predates libavif's 3.22 minimum, so the static image codec build
# tools come from PyPI.
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  set -eu; \
  sed -i 's|http://deb.debian.org/debian|https://archive.debian.org/debian|g; s|http://security.debian.org/debian-security|https://archive.debian.org/debian-security|g' /etc/apt/sources.list \
  && apt-get -o Acquire::Check-Valid-Until=false update \
  && apt-get install -y --no-install-recommends \
    libssl-dev \
    nasm \
    pkg-config \
    python3-pip \
  && rm -rf /var/lib/apt/lists/* \
  && if [ -s /run/secrets/proxy_ca ]; then export PIP_CERT=/run/secrets/proxy_ca; fi \
  && python3 -m pip install --no-cache-dir --disable-pip-version-check \
    'cmake>=3.22,<4' 'meson>=0.54' ninja

WORKDIR /workspace
