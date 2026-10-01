# SPDX-License-Identifier: GPL-2.0-only
# Exact runtime-fixture extension; private disposable test trust only.
ARG FIXTURE_IMAGE
FROM ${FIXTURE_IMAGE}
USER 0:0
RUN apt-get update && apt-get install -y --no-install-recommends python3 openssl \
    && apt-get clean \
    && openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
       -subj /CN=subtitle-fixture-ca \
       -keyout /fixtures/subtitle-ca-key.pem -out /fixtures/subtitle-ca.pem 2>/dev/null \
    && openssl req -new -newkey rsa:2048 -nodes -subj /CN=subtitle-fixture \
       -addext subjectAltName=IP:11.255.255.1 -addext basicConstraints=CA:FALSE \
       -addext extendedKeyUsage=serverAuth \
       -keyout /fixtures/subtitle-key.pem -out /fixtures/subtitle.csr 2>/dev/null \
    && openssl x509 -req -in /fixtures/subtitle.csr -days 1 -copy_extensions copy \
       -CA /fixtures/subtitle-ca.pem -CAkey /fixtures/subtitle-ca-key.pem -CAcreateserial \
       -out /fixtures/subtitle-server.pem \
    && cat /fixtures/subtitle-ca.pem >> /etc/ssl/certs/ca-certificates.crt
