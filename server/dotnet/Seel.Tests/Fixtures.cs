using System.Text.Json.Nodes;

namespace Seel.Tests;

/// <summary>The shared fixtures in server/, which every port's suite reads.</summary>
internal static class Fixtures
{
    public static readonly string ServerDir = FindServerDir();

    public static readonly JsonObject Validation = Load("validation-cases.json");

    public static readonly JsonObject Binding = Load("binding-cases.json");

    public static JsonArray Section(JsonObject fixture, string name) => fixture[name]!.AsArray();

    public static JsonObject Case(JsonObject fixture, string section, string name) =>
        Section(fixture, section).Single(c => (string)c!["name"]! == name)!.AsObject();

    public static TheoryData<string> Names(JsonObject fixture, string section) =>
        [.. Section(fixture, section).Select(c => (string)c!["name"]!)];

    public static JsonObject CompletePayload(string operation) =>
        Case(Validation, "cases", $"{operation}: complete payload")["payload"]!.DeepClone().AsObject();

    private static JsonObject Load(string file) =>
        JsonNode.Parse(File.ReadAllText(Path.Combine(ServerDir, file)))!.AsObject();

    // Walk up from the test binary so the suite runs from any working directory.
    private static string FindServerDir()
    {
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir is not null; dir = dir.Parent)
        {
            if (File.Exists(Path.Combine(dir.FullName, "validation-cases.json")))
            {
                return dir.FullName;
            }
        }
        throw new FileNotFoundException("server/validation-cases.json not found above " + AppContext.BaseDirectory);
    }
}
