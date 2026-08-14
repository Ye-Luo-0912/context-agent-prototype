// Minimized from vercel/ms before 9992f8a: regex result named m shadows minutes.

var s = 1000;
var m = s * 60;
var h = m * 60;

function parse(str) {
  str = String(str);
  var m = /^((?:\d+)?\.?\d+) *(ms|seconds?|s|minutes?|m|hours?|h)?$/i.exec(str);
  if (!m) {
    return;
  }
  var n = parseFloat(m[1]);
  var type = (m[2] || "ms").toLowerCase();
  switch (type) {
    case "hours":
    case "hour":
    case "h":
      return n * h;
    case "minutes":
    case "minute":
    case "m":
      return n * m;
    case "seconds":
    case "second":
    case "s":
      return n * s;
    case "ms":
      return n;
    default:
      return undefined;
  }
}

module.exports = parse;
