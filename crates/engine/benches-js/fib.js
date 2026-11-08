// Function calls and arithmetic: the tree-walker's call path.
function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
var r = fib(22);
if (r !== 17711) throw new Error("fib " + r);
