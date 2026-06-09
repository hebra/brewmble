COMPONENTS := cli daemon rest web
REPO_URL := https://codeberg.org/heb/brewmble.git

.PHONY: all build test clean install install-cli install-daemon $(COMPONENTS)

all: build

build:
	@for component in $(COMPONENTS); do \
		echo "Building $$component..."; \
		$(MAKE) -C $$component build; \
	done

test:
	@for component in $(COMPONENTS); do \
		echo "Testing $$component..."; \
		$(MAKE) -C $$component test; \
	done

clean:
	@for component in $(COMPONENTS); do \
		echo "Cleaning $$component..."; \
		$(MAKE) -C $$component clean; \
	done

install: install-cli install-daemon

install-cli:
	cargo install --git $(REPO_URL) brewmble --force

install-daemon:
	cargo install --git $(REPO_URL) brewmbled --force
