WASM_DIRS := $(sort $(patsubst %/Cargo.toml,%,$(wildcard examples/wasm-*/Cargo.toml plugins/*/Cargo.toml runtimes/*/Cargo.toml)))
WASM_JOBS := $(addprefix build-wasm-,$(WASM_DIRS))

.PHONY: all host wasm local-runtime local-client-package $(WASM_JOBS)

all: local-runtime

local-runtime: host wasm
	@bash $(BUILD_TOOL) --assemble-local-runtime

host:
	@bash $(BUILD_TOOL) --build-host

wasm: local-client-package

# Serial workspace entry avoids concurrent Cargo target locks and duplicate
# dependencies; Cargo still uses the shared jobserver inside each workspace.
wasm_previous :=
$(foreach job,$(WASM_JOBS),$(eval $(job): $(wasm_previous))$(eval wasm_previous := $(job)))
local-client-package: $(WASM_JOBS)
	@bash $(BUILD_TOOL) --assemble-local-client

$(WASM_JOBS):
	@bash $(BUILD_TOOL) --build-wasm-one $(patsubst build-wasm-%,%,$@)
