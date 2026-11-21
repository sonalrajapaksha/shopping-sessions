IMAGE ?= shopping-sessions:latest
CPUS  ?= 8
MEM   ?= 24g
# The scorer's platform. PLATFORM=linux/arm64 builds and runs natively on an Apple-silicon Mac, for fast local loops.
PLATFORM ?= linux/amd64
DEV   ?= dev
# In the monorepo the fixtures live two levels up; the published starter has its own copy.
FIXTURES ?= $(if $(wildcard fixtures),fixtures,../../fixtures)
export IMAGE CPUS MEM PLATFORM

.PHONY: build fixtures dev run-dev check-dev

## Build the image for the scorer's platform, linux/amd64 (emulated on Apple silicon)
build:
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .

## Run the image on all 36 edge-case fixtures and diff against the expected tables
fixtures: build
	$(PYTHON) scripts/run_fixtures.py $(FIXTURES) -- scripts/docker_run.sh

## Make the 1M-event dev set with the published generator (needs `generate` on PATH)
dev: generator
	$(GENERATOR) --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

## Run on the dev set with scorer flags and time it
run-dev: build
	rm -rf out && time scripts/docker_run.sh $(DEV) out

## Compare your dev output with the published dev answer (dev-answer.zip, unzipped): make check-dev ANSWER=dev-answer
ANSWER ?= data/dev-answer
check-dev:
	$(PYTHON) scripts/compare.py out $(ANSWER)

# Local tooling; original starter targets above remain available.
PYTHON ?= .venv/bin/python
GENERATOR := tools/generator/generator/target/release/generate
SEED ?= 1
DATASET ?= data/small
REPEATS ?= 3
TIMEOUT ?= 3600

.PHONY: setup generator generate-small generate-medium generate-dev generate-performance generate-large test-fixtures benchmark clean
setup:
	python3 -m venv .venv
	.venv/bin/pip install -r requirements-dev.txt

generator:
	cargo build --release --locked --manifest-path tools/generator/generator/Cargo.toml

generate-small: generator
	$(GENERATOR) --seed $(SEED) --events 10000 --clicks 2500 --output data/small

generate-medium: generator
	$(GENERATOR) --seed $(SEED) --events 100000 --clicks 25000 --output data/medium

generate-dev: generator
	$(GENERATOR) --seed $(SEED) --events 1000000 --clicks 250000 --output dev

generate-performance: generator
	$(GENERATOR) --seed $(SEED) --events 5000000 --clicks 1250000 --output data/performance

generate-large: generator
	$(GENERATOR) --seed $(SEED) --events 20000000 --clicks 5000000 --output data/large

test-fixtures: build
	$(PYTHON) scripts/run_fixtures.py $(FIXTURES) -- scripts/docker_run.sh

benchmark:
	$(PYTHON) benchmarks/benchmark.py --data $(DATASET) --image $(IMAGE) --repeats $(REPEATS) --timeout $(TIMEOUT) --seed $(SEED)

clean:
	@echo 'Removing generated datasets, outputs, benchmark results, and Rust build artifacts.'
	rm -rf data/small data/medium data/performance data/large dev out output scratch benchmarks/results tools/generator/generator/target

.PHONY: fetch-answer
fetch-answer:
	@mkdir -p data
	curl -fL --retry 2 -o data/dev-answer.zip https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/dev-answer.zip
	unzip -tq data/dev-answer.zip
	unzip -oq data/dev-answer.zip -d data
