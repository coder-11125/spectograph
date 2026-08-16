// Class test
class Greeter {
    constructor(name) {
        this.name = name;
    }

    greet() {
        return `Hello, ${this.name}!`;
    }

    // Unused method
    unusedMethod() {
        return "Should be dead";
    }
}

class ChildGreeter extends Greeter {
    constructor(name) {
        super(name);
    }

    greet() {
        return `Hi, ${this.name}!`;
    }

    // Unused child method
    unusedChildMethod() {
        return "Dead code";
    }
}

const greeter = new Greeter("World");
console.log(greeter.greet());

const child = new ChildGreeter("Universe");
console.log(child.greet());
