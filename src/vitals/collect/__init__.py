"""One module per subsystem. Each has ``static()`` for what the machine is
and ``live(...)`` for what it is doing; neither raises for a reading it
cannot take, which is None or absent, never 0."""
