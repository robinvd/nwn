import socket
import sys
import json
import os
import importlib.util, importlib.machinery, importlib.abc
import traceback
from typing import Optional

class MemoryLoader(importlib.abc.FileLoader):
    def __init__(self, path: str, code: bytes):
        self._path = path
        self._code = code

    def get_source(self, fullname):
        return self._code

    def get_filename(self, fullname: str):
        return self._path

    def get_data(self, path) -> bytes:
        return self._code


def get_stack_trace():
    stack_summary = traceback.extract_stack()

    return [{
        "file": frame.filename,
        "line": frame.lineno,
    } for frame in stack_summary]


def show(*args):
    trace_data = get_stack_trace()

    data = {
        "frames": trace_data,
        "out": " ".join(str(arg) for arg in args),
    }
    json_data = json.dumps(data)
    msg_data = (json_data + "\n").encode("utf8")

    sock.sendall(msg_data)





def import_from_string(code: bytes, path: str):
    module_name = "main"
    loader = MemoryLoader(path, code)
    spec = importlib.machinery.ModuleSpec(module_name, loader)
    module = importlib.util.module_from_spec(spec)

    module.show = show

    module.__loader__.exec_module(module)


def handle_message(data: bytes, path: str):
    import_from_string(data, path)


sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)

connection_path = os.environ["NWN_CONNECTION_FD"]
backing_file_path = os.environ["NWN_FILE_PATH"]

sock.connect(connection_path)

all_data = bytearray()

while True:
    data = sock.recv(4096)

    if not data:
        break

    all_data.extend(data)
    if len(all_data) > 0 and all_data[-1] == 0:
        handle_message(all_data[:-1], backing_file_path)
        break