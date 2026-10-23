.PHONY: build run check fmt clippy test clean container

CONTAINER_TOOL ?= podman
IMAGE_NAME ?= brewmbled

build:
	cargo build

run:
	cargo run

check:
	cargo check

fmt:
	cargo fmt

clippy:
	cargo clippy -- -D warnings

test:
	cargo test

clean:
	cargo clean

container:
	$(CONTAINER_TOOL) build -t $(IMAGE_NAME) -f Containerfile .
	# mDNS discovery requires host networking on Linux (or an mDNS reflector).
	$(CONTAINER_TOOL) run --rm --network host -p 4712:4712 -p 5353:5353/udp --name brewmbled $(IMAGE_NAME)
