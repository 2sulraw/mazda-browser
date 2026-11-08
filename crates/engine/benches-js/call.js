function f() { return 1 }
var s = 0;
for (var i = 0; i < 300000; i++) s += f();
if (s !== 300000) throw new Error("s " + s);
