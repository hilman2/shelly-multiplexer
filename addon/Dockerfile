ARG BUILD_FROM
# ---- Builder stage --------------------------------------------------------
# We build inside a Rust image whose architecture matches the target. The
# Home Assistant build system invokes us once per architecture with the
# matching BUILD_FROM, so a plain `cargo build` ends up with the right
# binary without any cross-toolchain juggling.
FROM rust:1-bookworm AS builder
WORKDIR /src

# Plain build with sources copied in one go. The previous attempt at a
# split dependency-prebuild was producing a binary built from the
# placeholder `fn main() {}` instead of our real source — Cargo doesn't
# always re-resolve targets when the dummy main/lib are replaced after
# a prior compile. Build incrementality matters less than correctness.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin shelly-multiplexer && \
    strip target/release/shelly-multiplexer && \
    ls -lh target/release/shelly-multiplexer

# ---- Runtime stage --------------------------------------------------------
FROM ${BUILD_FROM}

ENV LANG=C.UTF-8

# HA base images already provide bashio + s6-overlay (which acts as
# init / signal forwarder), so we don't add tini. Only install what
# we actually need beyond the base.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/shelly-multiplexer /usr/bin/shelly-multiplexer
COPY addon/run.sh /usr/bin/run.sh
RUN chmod a+x /usr/bin/run.sh /usr/bin/shelly-multiplexer

CMD ["/usr/bin/run.sh"]

LABEL \
    io.hass.name="Shelly Multiplexer" \
    io.hass.description="Splits one Shelly Pro 3EM CT measurement across multiple battery storage systems with saldierende Lastverteilung." \
    io.hass.type="addon" \
    io.hass.version="0.1.0"
