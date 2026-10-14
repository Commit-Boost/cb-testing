# Runtime images for the epbs-stream harness. build-images.sh builds helix-relay and
# gloas_inject on the host and copies them, with ../stub/simstub.py, into the build
# context. The host build links glibc 2.39 and OpenSSL 3, so the base matches Ubuntu 24.04.
FROM ubuntu:24.04 AS base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3t64 python3 curl && rm -rf /var/lib/apt/lists/*
WORKDIR /app

FROM base AS relay
COPY helix-relay /app/
ENTRYPOINT ["/app/helix-relay"]

FROM base AS inject
COPY gloas_inject /app/
ENTRYPOINT ["/app/gloas_inject"]

FROM base AS stub
COPY simstub.py /app/
ENTRYPOINT ["python3", "-u", "/app/simstub.py", "8545"]
