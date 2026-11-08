// Property read/write on a plain object (linear Vec scan today).
var o = { a: 0, b: 2, c: 3 };
var sum = 0;
for (var i = 0; i < 300000; i++) {
  sum += o.a + o.b + o.c;
  o.a = 1;
}
if (sum !== 1799999) throw new Error("sum " + sum);
