class Newsletter:
    def publish(self):
        return "draft"


def run(n: Newsletter):
    return n.publish()
