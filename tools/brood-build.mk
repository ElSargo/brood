WASM_DIRS := $(sort $(patsubst %/Cargo.toml,%,$(wildcard examples/wasm-*/Cargo.toml plugins/*/Cargo.toml)))
WASM_JOBS := $(addprefix build-wasm-,$(WASM_DIRS))

.PHONY: all host wasm local-client-package $(WASM_JOBS)

all: host wasm

host:
	@bash $(BUILD_TOOL) --build-host

wasm: local-client-package

local-client-package: $(WASM_JOBS)
	@bash $(BUILD_TOOL) --assemble-local-client

$(WASM_JOBS):
	@bash $(BUILD_TOOL) --build-wasm-one $(patsubst build-wasm-%,%,$@)
