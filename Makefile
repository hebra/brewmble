COMPONENTS := cli daemon rest web
REPO_URL := https://codeberg.org/heb/brewmble.git

.PHONY: all build test clean install install-cli install-daemon bump-version $(COMPONENTS)

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

bump-version:
	@version="$(word 1,$(filter-out $@,$(MAKECMDGOALS)))"; \
	if [ -z "$$version" ]; then \
		echo "Usage: make bump-version <version>"; \
		exit 1; \
	fi; \
	if ! echo "$$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$'; then \
		echo "Error: version must be in semver format (e.g., 1.2.3)"; \
		exit 1; \
	fi; \
	for cargo_toml in cli/Cargo.toml daemon/Cargo.toml rest/Cargo.toml; do \
		sed -e 's/^version = "[^"]*"/version = "'"$$version"'"/' "$$cargo_toml" > "$$cargo_toml.tmp" && mv "$$cargo_toml.tmp" "$$cargo_toml"; \
	done; \
	for lock_file in cli/Cargo.lock daemon/Cargo.lock rest/Cargo.lock; do \
		case "$$lock_file" in \
			cli/Cargo.lock) \
				sed -e '/^name = "brewmble"$$/{n;s/^version = "[^"]*"/version = "'"$$version"'"/;}' \
				    -e '/^name = "brewmble-rest"$$/{n;s/^version = "[^"]*"/version = "'"$$version"'"/;}' \
				    "$$lock_file" > "$$lock_file.tmp" && mv "$$lock_file.tmp" "$$lock_file"; \
				;; \
			daemon/Cargo.lock) \
				sed -e '/^name = "brewmbled"$$/{n;s/^version = "[^"]*"/version = "'"$$version"'"/;}' \
				    -e '/^name = "brewmble-rest"$$/{n;s/^version = "[^"]*"/version = "'"$$version"'"/;}' \
				    "$$lock_file" > "$$lock_file.tmp" && mv "$$lock_file.tmp" "$$lock_file"; \
				;; \
			rest/Cargo.lock) \
				sed -e '/^name = "brewmble-rest"$$/{n;s/^version = "[^"]*"/version = "'"$$version"'"/;}' \
				    "$$lock_file" > "$$lock_file.tmp" && mv "$$lock_file.tmp" "$$lock_file"; \
				;; \
		esac; \
	done; \
	echo "Bumped version to $$version"

%:
	@:
