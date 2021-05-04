type Message = SimpleOutput | RegexOutput | RawOutput;

type SimpleOutput = {
    frames: Frame[],
    out: string,
    kind: "output" | "debug" | "error" | null,
}

type RegexOutput = {
    frames: Frame[],
    out: string,
    regex: string,
}

type RawOutput = {
    changes: TextEdit[],
}

type Frame = {
    file: string,
    line: number,
}