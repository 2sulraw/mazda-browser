// Control loop: arithmetic and branch overhead without property access.
var sum = 0;
for (var i = 0; i < 300000; i++) sum += i;
if (sum !== 44999850000) throw new Error("sum " + sum);
