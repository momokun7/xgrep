from typing import Optional

class SearchResult:
    file: str
    line_number: int
    line: str

class IndexStatus:
    state: str
    changed_files: Optional[int]
    indexed_files: int
    index_size_bytes: int
    index_path: str

class Xgrep:
    @staticmethod
    def open(root: str) -> Xgrep: ...
    @staticmethod
    def open_local(root: str) -> Xgrep: ...
    def build_index(self) -> None: ...
    def search(
        self,
        pattern: str,
        *,
        case_insensitive: bool = ...,
        regex: bool = ...,
        file_type: Optional[str] = ...,
        max_count: Optional[int] = ...,
        changed_only: bool = ...,
        since: Optional[str] = ...,
        path_pattern: Optional[str] = ...,
        fresh: bool = ...,
        word: bool = ...,
        globs: Optional[list[str]] = ...,
    ) -> list[SearchResult]: ...
    def index_status(self) -> IndexStatus: ...
    @property
    def root(self) -> str: ...
    @property
    def index_path(self) -> str: ...
