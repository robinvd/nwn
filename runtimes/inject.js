const net = require('net')

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
        .reverse()

    return stackData;
}

function requireFromString(src, filename) {
    var Module = module.constructor;
    var m = new Module();
    m._compile(src, filename);
    return m.exports;
}

function handleMessage(data) {
    console.log('handle msg');
    requireFromString(data, realFile)
}

const client = net.createConnection(connectionPath);

const sendOutput = (kind, out) => {
    console.log("send started");
    const stackData = getStackTrace();
    const data = {
        frames: stackData,
        out: out,
        kind: kind,
    }
    const output_data = JSON.stringify(data) + '\n';
    client.write(output_data)
}


global.show = (...args) => {
    const out = args.join(" ")
    sendOutput("output", out)
}
global.debug = (...args) => {
    const out = args.join(" ")
    sendOutput("debug", out)
}

let contents = "";

client.on('data', (data) => {
    contents += data;

    if (contents[contents.length - 1] === '\u{0}') {
        contents = contents.slice(0, -1);
        handleMessage(contents)
        client.end()
    }
})

client.on('end', () => {
    console.log("end!")
})