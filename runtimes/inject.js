const net = require('net')
const log = console.log;

log("test")
const connectionPath = process.env.NWN_CONNECTION_FD;
const realFile = process.env.NWN_FILE_PATH

function getStackTrace() {
    const stack = new Error().stack;
    const stackData = stack
        .split('\n')
        .slice(1)
        .map((line) => /\(([^:]+):(\d+):\d+\)/.exec(line))
        .filter((match) => match !== null)
        .map((match) => { return { file: match[1], line: Number(match[2]) } })

    return stackData;
    return data
}

function requireFromString(src, filename) {
    var Module = module.constructor;
    var m = new Module();
    m._compile(src, filename);
    return m.exports;
}

function handleMessage(data) {
    log('handle msg');
    requireFromString(data, realFile)
}

const client = net.createConnection(connectionPath);

console.log = (...args) => {
    log("console log started");
    const stackData = getStackTrace();
    const data = {
        frames: stackData,
        out: args.join(" "),
    }
    const output_data = JSON.stringify(data) + '\n';
    log('output data', output_data);
    // process.stderr.write(JSON.stringify(data) + '\n');
    // log(...args);
    client.write(output_data)
}

let contents = "";

client.on('data', (data) => {
    contents += data;

    log(JSON.stringify(contents));
    log(contents[contents.length - 1], contents[contents.length - 1] === '\u{0}');
    log(typeof(contents))

    if (contents[contents.length - 1] === '\u{0}') {
        contents = contents.slice(0, -1);
        handleMessage(contents)
        client.end()
    }
})

client.on('end', () => {
    log("end!")
})