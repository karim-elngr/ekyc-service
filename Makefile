.PHONY: up down smoke mocks test
up:
	docker compose up --build -d
down:
	docker compose down
smoke:
	python3 scripts/smoke.py
mocks:
	python3 -m unittest discover -s mocks -v
test:
	cargo test --locked --workspace
	python3 -m unittest discover -s mocks -v
