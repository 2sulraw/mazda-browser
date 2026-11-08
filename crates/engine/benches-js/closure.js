// Closure allocation and captured-variable writes.
function make() {
  var n = 0;
  return function () {
    n++;
    return n;
  };
}
var f = make();
var last = 0;
for (var i = 0; i < 300000; i++) last = f();
if (last !== 300000) throw new Error("last " + last);
