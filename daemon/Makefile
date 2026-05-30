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
	$(CONTAINER_TOOL) run --rm --network bridge -p 8080:8080 -p 5353:5353 --name brewmbled $(IMAGE_NAME)
