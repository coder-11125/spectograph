// Parent class with methods
export class Parent {
    constructor(name) {
        this.name = name;
    }

    greet() {
        return `Hello, I'm ${this.name}`;
    }

    farewell() {
        return `Goodbye from ${this.name}`;
    }

    // Unused method in parent
    unusedMethod() {
        return "This should be dead";
    }
}