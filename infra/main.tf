module "foo" {
    source = "./foo"

    length = length(module.bar["x"].digest)
}

module "bar" {
    for_each = {
        x = 2
        y = 3
        z = 5
    }
    source = "./bar"
}