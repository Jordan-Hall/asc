namespace Demo.App;

/// <summary>A thing.</summary>
public class Widget : IWidget
{
    private int count;
    public string Name { get; set; }

    public Widget(string name) { Name = name; }

    /// <summary>Run it.</summary>
    public void Run()
    {
        Helper(count);
        var w = new Widget("x");
    }

    private static int Helper(int n) => n + 1;
}

public interface IWidget { void Run(); }

public enum Mode { Fast, Slow }

public record Pair(int A, int B);

public struct Vec { public int X; }
