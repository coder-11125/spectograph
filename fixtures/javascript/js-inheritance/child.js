// Child class that extends Parent
import { Parent } from './parent.js';

class Child extends Parent {
    constructor(name) {
        super(name);
    }

    greet() {
        return super.greet() + " (child)";
    }

    play() {
        return `${this.name} is playing`;
    }

    // Unused method in child
    unusedChildMethod() {
        return "This should be dead";
    }
}

// Main entry point
function main() {
    const child = new Child("Alice");
    console.log(child.greet());
    console.log(child.play());
}

main();