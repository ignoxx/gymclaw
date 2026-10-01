class DomainError(ValueError):
    """Stable machine-readable domain failure, not an integration exception."""
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
