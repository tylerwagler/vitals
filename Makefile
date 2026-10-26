# vitals.pyz: the whole package as one file that runs with any python3 >= 3.11,
#   python3 dist/vitals.pyz --port 9420
# A pynvml.py copied next to it is importable (containers without nvidia-ml-py).

PYTHON ?= python3

.PHONY: pyz test clean
pyz: dist/vitals.pyz

dist/vitals.pyz: $(shell find src/vitals -name '*.py')
	rm -rf build/pyz && mkdir -p build/pyz dist
	cp -r src/vitals build/pyz/
	find build/pyz -name __pycache__ -prune -exec rm -rf {} +
	$(PYTHON) -m zipapp build/pyz -m 'vitals.cli:main' -p '/usr/bin/env python3' -c -o $@
	@echo "built $@ ($$(stat -c %s $@) bytes)"

test:
	$(PYTHON) -m pytest -q

clean:
	rm -rf build dist
