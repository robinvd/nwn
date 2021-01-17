const fs = require('fs')

const log = console.log;

console.log = (...args) => {
    const stack = new Error().stack;
    const stackData = stack
        .split('\n')
        .slice(1)
        .map((line) => /\(([^:]+):(\d+):\d+\)/.exec(line))
        .filter((match) => match !== null)
        .map((match) => { return { file: match[1], line: Number(match[2]) } })
    const data = {
        frames: stackData,
        out: args.join(" "),
    }
    process.stderr.write(JSON.stringify(data) + '\n');

    log(...args);
}

function requireFromString(src, filename) {
    var Module = module.constructor;
    var m = new Module();
    m._compile(src, filename);
    return m.exports;
}

const realFile = process.env.NWN_FILE_PATH

fs.readFile(process.stdin.fd, (err, text) => {
    if (!err) {
        requireFromString(String(text), realFile)
    } else {
        log("error loading file:", err)
    }
})