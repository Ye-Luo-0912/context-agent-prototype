using Avalonia.Controls;
using FocusAgent.Desktop.ViewModels;

namespace FocusAgent.Desktop;

public partial class MainWindow : Window
{
    public MainWindow()
    {
        InitializeComponent();
        // N4/F13: closing the window cancels the view model's lifetime —
        // the event pump, the fallback timer, every pending wait and the
        // session itself are released through one path.
        Closed += (_, _) =>
        {
            if (DataContext is MainWindowViewModel viewModel)
            {
                _ = viewModel.DisposeAsync();
            }
        };
    }
}
