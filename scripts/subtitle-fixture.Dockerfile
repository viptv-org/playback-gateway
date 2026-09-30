# SPDX-License-Identifier: GPL-2.0-only
# Exact runtime-fixture extension; private disposable test trust only.
ARG FIXTURE_IMAGE
FROM ${FIXTURE_IMAGE}
USER 0:0
RUN apt-get update && apt-get install -y --no-install-recommends python3 openssl \
    && apt-get clean \
    && openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
       -subj /CN=subtitle-fixture -addext subjectAltName=IP:11.255.255.1 \
       -keyout /fixtures/subtitle-key.pem -out /fixtures/subtitle-ca.pem \
    && cat /fixtures/subtitle-ca.pem >> /etc/ssl/certs/ca-certificates.crt
