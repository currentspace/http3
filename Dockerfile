ARG NODE_IMAGE=node:24.22.1-trixie-slim@sha256:89c5244bc22a9bc3287d89ebc7e49b9cb5bb84cd40e6175ac3421d5a9f68b95a
FROM amazon/aws-cli:2.39.0@sha256:6014c77975a4ae237395924cc821333f20bf87c91964a45dd4ed5f3b13b424f5 AS aws-cli

FROM ${NODE_IMAGE} AS native-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  curl \
  build-essential \
  pkg-config \
  cmake \
  clang \
  perl \
  git \
  && rm -rf /var/lib/apt/lists/*

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain 1.99.1
ENV PATH="/root/.cargo/bin:${PATH}"
ENV CARGO_BUILD_JOBS=8
RUN npm install -g pnpm@12.16.1

WORKDIR /app

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY examples/hono/package.json ./examples/hono/package.json
COPY examples/express-adapter/package.json ./examples/express-adapter/package.json
COPY examples/workerd-client/package.json ./examples/workerd-client/package.json

RUN pnpm install --frozen-lockfile

# Keep the native+TS build cache stable when only example app files change.
COPY build.rs Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY benches ./benches
COPY fuzz ./fuzz
COPY tools ./tools
COPY crates ./crates
COPY spikes/quiche-wasm-wasip1 ./spikes/quiche-wasm-wasip1
COPY vendor ./vendor
COPY src ./src
COPY lib ./lib
COPY scripts ./scripts
COPY index.js index.d.ts tsconfig.json ./

RUN pnpm run build
RUN pnpm prune --prod

# acme.sh is immutable build input; startup never downloads executable code.
RUN curl -fsSL "https://codeload.github.com/acmesh-official/acme.sh/tar.gz/807da6498377ee5e0cf43a78091f46f12dc59a89" -o /tmp/acme.tar.gz \
    && echo "ddbe1bcbd1a44a2623a2af167ebdc678669e6e2eb396742f2d1d28e02dc14220  /tmp/acme.tar.gz" | sha256sum --check \
    && mkdir -p /opt/acme.sh \
    && tar -xzf /tmp/acme.tar.gz -C /opt/acme.sh --strip-components=1 \
    && rm /tmp/acme.tar.gz \
    && /opt/acme.sh/acme.sh --version

FROM ${NODE_IMAGE} AS hono-deps

ENV CARGO_BUILD_JOBS=8
RUN npm install -g pnpm@12.16.1

WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY examples/hono/package.json ./examples/hono/package.json
RUN pnpm --filter http3-hono-example install --prod --frozen-lockfile

FROM ${NODE_IMAGE} AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  curl \
  bash \
  openssl \
  python3 \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
ENV NODE_ENV=production
ENV LE_WORKING_DIR=/opt/acme.sh LE_CONFIG_HOME=/var/run/http3-acme

COPY --from=native-builder /opt/acme.sh /opt/acme.sh
RUN ln -s /opt/acme.sh/acme.sh /usr/local/bin/acme.sh
COPY --from=aws-cli /usr/local/aws-cli /usr/local/aws-cli
RUN ln -s /usr/local/aws-cli/v2/current/bin/aws /usr/local/bin/aws && aws --version

COPY --from=native-builder /app/package.json /app/pnpm-lock.yaml ./
COPY --from=native-builder /app/index.js /app/index.d.ts ./
COPY --from=native-builder /app/dist ./dist
COPY --from=native-builder /app/*.node ./
COPY --from=native-builder /app/node_modules ./node_modules

COPY examples/hono ./examples/hono
COPY examples/certs ./examples/certs
COPY --from=hono-deps /app/examples/hono/node_modules ./examples/hono/node_modules

RUN chmod +x ./examples/hono/acme-entrypoint.sh

EXPOSE 443/tcp 443/udp 8080/tcp

CMD ["bash", "examples/hono/acme-entrypoint.sh"]
