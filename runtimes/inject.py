import socket
import ast
import json
import os
import importlib.util, importlib.machinery, importlib.abc
import traceback


root_ast = None


class ArgFinder(ast.NodeVisitor):
    def __init__(self, *args, name, lineno, **kwargs):
        self._name = name
        self._lineno = lineno
        self._result = None
        super().__init__(*args, **kwargs)

    def visit(self, node):
        super().visit(node)
        return self._result

    def visit_Call(self, node: ast.Call):
        if getattr(node.func, "id", None) != self._name or node.lineno != self._lineno:
            return self.generic_visit(node)

        self._result = node
        return node


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


def send_msg(data):
    json_data = json.dumps(data)
    msg_data = (json_data + "\n").encode("utf8")

    sock.sendall(msg_data)


def show(*args):
    data = {
        "frames": get_stack_trace(),
        "out": " ".join(str(arg) for arg in args),
    }
    send_msg(data)


def do(val, _result=None):
    stack_data = get_stack_trace()
    data = {
        "frames": stack_data,
        "out": str(val),
        "insert": "last_arg",
    }

    send_msg(data)

    return val


def sheet(user_data):
    stack_data = get_stack_trace()
    calling_code_line = stack_data[-3]["line"]

    try:
        call_ast: ast.Call = ArgFinder(name="sheet", lineno=calling_code_line).visit(root_ast)
        assert isinstance(call_ast, ast.Call)
        user_data_ast = call_ast.args[0]

        if not isinstance(user_data_ast, ast.List):
            return

        changes = []
        for row_ast, row in zip(user_data_ast.elts, user_data):
            if not isinstance(row_ast, ast.Dict):
                continue

            for value_ast, (key, value) in zip(row_ast.values, row.items()):
                if callable(key):
                    try:
                        new_value = key(user_data, row)
                    except Exception as e:
                        print(e)
                        continue
                    changes.append({
                        "range": {
                            "start": {"line": value_ast.lineno - 1, "character": value_ast.col_offset},
                            "end": {"line": value_ast.end_lineno - 1, "character": value_ast.end_col_offset},
                        },
                        "newText": str(new_value),
                    })
    except Exception as e:
        print(e)
        return

    data = {
        "frames": stack_data,
        "changes": changes,
    }

    send_msg(data)

    return None


def import_from_string(code: bytes, path: str):
    module_name = "main"
    loader = MemoryLoader(path, code)
    spec = importlib.machinery.ModuleSpec(module_name, loader)
    module = importlib.util.module_from_spec(spec)
    global root_ast
    root_ast = ast.parse(code)

    module.show = show
    module.do = do
    module.sheet = sheet

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