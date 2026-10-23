// Adds two numbers.
function add(a, b) {
  return a + b;
}

class Counter {
  count = 0;
  inc() {
    this.count = add(this.count, 1);
  }
}

const twice = (x) => add(x, x);
var legacy = 1;
