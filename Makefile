PYTHON ?= python3
.PHONY: up down smoke mocks test verify unit schema-refresh
up:
	docker compose --profile portal up --build -d
down:
	docker compose --profile portal down
smoke:
	$(PYTHON) scripts/product_smoke.py
	$(PYTHON) scripts/interactive_smoke.py
	$(PYTHON) scripts/queries_smoke.py
mocks:
	$(PYTHON) -m unittest discover -s mocks -v
test: verify
verify:
	$(PYTHON) scripts/verify.py
unit:
	cargo test --locked --workspace
	$(PYTHON) -m unittest discover -s mocks -v
	cd apps/portal && pnpm test
schema-refresh:
	$(PYTHON) scripts/export_schema.py
