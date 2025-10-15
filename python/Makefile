.PHONY: venv
venv:
	if [ ! -f .venv/bin/python ] ; then uv venv --no-project .venv ; fi
	UV_PYTHON=.venv/bin/python UV_PROJECT_ENVIRONMENT=.venv uv pip install -r requirements.txt --quiet

.PHONY: lint
lint: venv
	uvx ruff check .
	uvx ruff format --check .
	.venv/bin/pyright . --pythonpath .venv/bin/python

.PHONY: format
format: venv
	uvx ruff check --fix .
	uvx ruff format .
