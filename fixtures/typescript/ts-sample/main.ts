function add(a: number, b: number): number {
    return a + b;
}

class Calculator {
    total: number = 0;

    reset(): void {
        this.total = 0;
    }
}

const calc = new Calculator();
calc.reset();

export function subtract(a: number, b: number): number {
    return a - b;
}
