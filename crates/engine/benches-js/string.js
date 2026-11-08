// String concatenation and charCodeAt on the result.
var s = "";
for (var i = 0; i < 2000; i++) s += "ab";
if (s.length !== 4000) throw new Error("length " + s.length);
var sum = 0;
for (var j = 0; j < s.length; j++) sum += s.charCodeAt(j);
if (sum !== 390000) throw new Error("sum " + sum);
