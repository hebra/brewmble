COMPONENTS := cli daemon rest web

.PHONY: all build test clean $(COMPONENTS)

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
