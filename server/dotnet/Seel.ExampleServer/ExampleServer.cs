using Seel.Client;

namespace Seel.ExampleServer;

public static class ExampleServer
{
    /// <summary>
    /// Build the server for a config. Program.cs passes the environment's;
    /// tests pass their own and port 0.
    /// </summary>
    public static WebApplication Build(ServerConfig config)
    {
        var builder = WebApplication.CreateSlimBuilder();
        builder.WebHost.UseUrls($"http://{config.Host}:{config.Port}");
        // Keep the startup line, drop ASP.NET's per-request info logs.
        builder.Logging.AddFilter("Microsoft.AspNetCore", LogLevel.Warning);
        builder.Services.AddSingleton(config);
        builder.Services.AddSingleton(config.Seel);
        builder.Services.AddHttpClient<SeelClient>();
        builder.Services.AddSingleton<IRetailerResolver>(new OriginRetailerResolver(config.Binding));
        builder.Services.AddTransient<ProxyHandler>();

        var app = builder.Build();
        app.Run(context => context.RequestServices.GetRequiredService<ProxyHandler>().HandleAsync(context));
        return app;
    }
}
