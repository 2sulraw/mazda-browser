// Dense array push, length read and indexed access.
var a = [];
for (var i = 0; i < 100000; i++) a.push(i);
var sum = 0;
for (var j = 0; j < a.length; j++) sum += a[j];
if (sum !== 4999950000) throw new Error("sum " + sum);
