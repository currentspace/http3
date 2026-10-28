ARG NODE_MAJOR=24.22.1
FROM node:${NODE_MAJOR}-bookworm AS native-builder

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
RUN npm install -g pnpm@12.16.1

WORKDIR /app

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY examples/hono/package.json ./examples/hono/package.json
COPY examples/express-adapter/package.json ./examples/express-adapter/package.json

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
COPY index.js index.d.ts tsconfig.json ./

RUN pnpm run build
RUN pnpm prune --prod

FROM node:${NODE_MAJOR}-bookworm AS hono-deps

RUN npm install -g pnpm@12.16.1

WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY examples/hono/package.json ./examples/hono/package.json
RUN pnpm --filter http3-hono-example install --prod --frozen-lockfile

FROM node:${NODE_MAJOR}-bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  curl \
  bash \
  openssl \
  python3 \
  awscli \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
ENV NODE_ENV=production

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
